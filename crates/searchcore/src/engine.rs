//! 引擎门面：Tauri / bench 统一入口。
//!
//! 启动流程：有快照 → 载入（秒级）→ NTFS 卷从 next_usn 追 USN 增量；
//! 无快照 → 全量构建（管理员 MFT 秒级 / 降级目录扫描）→ 落快照。
//! 之后常驻：每卷一个 USN 阻塞读线程，变更即时应用 + 节流保存快照。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;

use crate::index::{Index, VolumeIndex};
use crate::matcher::{self, SearchOptions, SearchOutcome};
use crate::mft;
use crate::snapshot;
use crate::usn::{self, UsnError};
use crate::volume;
use crate::walker;

#[derive(Clone, Debug)]
pub struct EngineStats {
    pub entries: usize,
    pub volumes: Vec<(char, bool, usize)>, // (盘符, is_ntfs, 条数)
    pub from_snapshot: bool,
    pub startup_ms: u128,
    pub is_admin: bool,
    pub usn_live: Vec<char>,
}

pub struct Engine {
    index: RwLock<Index>,
    /// 搜索代数：兼任"新查询使旧查询失效"的取消信号
    query_gen: AtomicU64,
    stop: AtomicBool,
    /// USN 线程代数：重建索引时自增，旧线程检测到变化后退出
    usn_gen: AtomicU64,
    snapshot_path: Mutex<PathBuf>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    pub stats: RwLock<EngineStats>,
    /// USN 线程累计应用的变更数（诊断用）
    pub applied_updates: AtomicU64,
}

/// HANDLE 的 Send 包装（Windows 句柄本身可跨线程使用）
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

/// 启动/升级诊断日志（追加到 LOCALAPPDATA\launcher\startup.log）。
pub fn slog(msg: &str) {
    use std::io::Write;
    let Ok(dir) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let p = std::path::Path::new(&dir).join("launcher").join("startup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// 开始菜单提权目录（系统全体 + 当前用户）
fn start_menu_paths() -> Vec<Vec<String>> {
    let mut paths = vec![vec![
        "ProgramData",
        "Microsoft",
        "Windows",
        "Start Menu",
        "Programs",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>()];
    if let Ok(home) = std::env::var("USERPROFILE") {
        // home 形如 C:\Users\xxx → 取 "Users\xxx" 段
        let segs: Vec<String> = home
            .split('\\')
            .filter(|s| !s.is_empty())
            .skip(1)
            .map(String::from)
            .collect();
        if !segs.is_empty() {
            let mut p = segs;
            p.extend(
                ["AppData", "Roaming", "Microsoft", "Windows", "Start Menu", "Programs"]
                    .iter()
                    .map(|s| s.to_string()),
            );
            paths.push(p);
        }
    }
    paths
}

/// 对所有 NTFS 卷标记开始菜单子树
fn mark_all_start_menu(index: &mut Index) {
    let paths = start_menu_paths();
    for v in index.volumes.iter_mut() {
        if v.is_ntfs {
            v.mark_start_menu(&paths);
        }
    }
}

fn close_h(h: HANDLE) {
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(h);
    }
}

/// MFT 重建一个 NTFS 卷并把日志游标对齐到当前，返回 (卷索引, 新句柄, journal_id, next_usn)。
fn rebuild_volume_aligned(letter: char) -> Option<(VolumeIndex, HANDLE, u64, i64)> {
    let t0 = Instant::now();
    let h = match mft::open_volume(letter) {
        Ok(h) => h,
        Err(e) => {
            slog(&format!("rebuild[{letter}]: open_volume 失败: {e}"));
            return None;
        }
    };
    let mut vi = VolumeIndex::new(letter, true);
    let n = match mft::scan_mft(h, &mut |frn, parent, name, is_dir| {
        if frn == 5 || name.is_empty() {
            return;
        }
        vi.upsert(frn, parent, name, is_dir);
    }) {
        Ok(n) => n,
        Err(e) => {
            close_h(h);
            slog(&format!("rebuild[{letter}]: scan_mft 失败: {e}"));
            return None;
        }
    };
    if let Err(e) = usn::ensure_journal(h) {
        close_h(h);
        slog(&format!("rebuild[{letter}]: ensure_journal 失败: {e:?}"));
        return None;
    }
    let (jid, _, next) = match usn::query_journal(h) {
        Ok(x) => x,
        Err(e) => {
            close_h(h);
            slog(&format!("rebuild[{letter}]: query_journal 失败: {e:?}"));
            return None;
        }
    };
    vi.journal_id = jid;
    vi.next_usn = next;
    slog(&format!(
        "rebuild[{}]: {} 条, {:.2}s",
        letter,
        n,
        t0.elapsed().as_secs_f64()
    ));
    Some((vi, h, jid, next))
}

/// 节流保存快照：锁内只做序列化（内存拷贝），校验和 + 写盘在锁外完成，
/// 避免 269MB 的哈希与磁盘 IO 长时间占住索引读锁、把并发搜索一起拖住。
fn save_snapshot_offlock(engine: &Engine) {
    let path = engine.snapshot_path.lock().unwrap().clone();
    let body = {
        let idx = engine.index.read().unwrap();
        snapshot::encode(&idx)
    };
    let _ = snapshot::write(body, &path);
}

/// USN 阻塞读循环：变更即时应用；脏且超 60 秒时保存快照；
/// 日志回绕时原地 MFT 重建并继续监听。`gen` 不匹配时退出（索引被重建）。
fn usn_loop(engine: Arc<Engine>, letter: char, handle: SendHandle, mut journal_id: u64, mut cursor: i64, gen: u64) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut handle = handle;
    let mut last_save = Instant::now();
    let mut dirty = false;
    const TIMEOUT_100NS: u64 = 50_000_000; // 5 秒

    loop {
        if engine.stop.load(Ordering::Relaxed) || engine.usn_gen.load(Ordering::SeqCst) != gen {
            break;
        }
        match usn::read_usn(handle.0, cursor, journal_id, TIMEOUT_100NS, 1, &mut buf) {
            Ok((records, next)) => {
                if !records.is_empty() {
                    let applied = {
                        let mut idx = engine.index.write().unwrap();
                        match idx.volumes.iter_mut().find(|v| v.letter == letter) {
                            Some(vi) => {
                                let n = usn::apply_updates(vi, &records);
                                vi.next_usn = next;
                                n
                            }
                            None => break,
                        }
                    };
                    if applied > 0 {
                        engine.applied_updates.fetch_add(applied as u64, Ordering::Relaxed);
                        dirty = true;
                    }
                }
                cursor = next;
                if dirty && last_save.elapsed() > Duration::from_secs(60) {
                    save_snapshot_offlock(&engine);
                    last_save = Instant::now();
                    dirty = false;
                }
            }
            Err(UsnError::Timeout) => {
                if dirty && last_save.elapsed() > Duration::from_secs(60) {
                    save_snapshot_offlock(&engine);
                    last_save = Instant::now();
                    dirty = false;
                }
            }
            Err(UsnError::JournalDeleted) => {
                // 日志被清/回绕：MFT 全量重建 + 游标重对齐，原地继续监听
                close_h(handle.0);
                match rebuild_volume_aligned(letter) {
                    Some((vi, h, jid, next)) => {
                        let mut idx = engine.index.write().unwrap();
                        idx.volumes.retain(|v| v.letter != letter);
                        idx.volumes.push(vi);
                        journal_id = jid;
                        cursor = next;
                        handle = SendHandle(h);
                    }
                    None => break,
                }
            }
            Err(UsnError::Io(_)) => {
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    close_h(handle.0);
}

impl Engine {
    pub fn new() -> Arc<Engine> {
        let snap_dir = std::env::var("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("launcher"))
            .unwrap_or_else(|_| PathBuf::from("."));
        Arc::new(Engine {
            index: RwLock::new(Index::new()),
            query_gen: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            usn_gen: AtomicU64::new(0),
            snapshot_path: Mutex::new(snap_dir.join("index.bin")),
            threads: Mutex::new(Vec::new()),
            stats: RwLock::new(EngineStats {
                entries: 0,
                volumes: Vec::new(),
                from_snapshot: false,
                startup_ms: 0,
                is_admin: false,
                usn_live: Vec::new(),
            }),
            applied_updates: AtomicU64::new(0),
        })
    }

    pub fn set_snapshot_path(&self, p: PathBuf) {
        *self.snapshot_path.lock().unwrap() = p;
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.snapshot_path.lock().unwrap().clone()
    }

    /// 启动/重建索引。必须在 Arc 下调用（USN 线程持有引擎引用）。
    pub fn startup(self: &Arc<Self>) -> EngineStats {
        let t0 = Instant::now();
        let is_admin = mft::open_volume('C').is_ok();
        let snap_path = self.snapshot_path();

        // 1) 尝试快照
        let mut loaded = false;
        let mut stale_vols: Vec<char> = Vec::new();
        if let Ok(idx) = snapshot::load_snapshot(&snap_path) {
            for v in &idx.volumes {
                if v.is_ntfs {
                    stale_vols.push(v.letter);
                }
            }
            let mut idx = idx;
            mark_all_start_menu(&mut idx);
            *self.index.write().unwrap() = idx;
            loaded = true;

            // 快照来自降级模式（无 MFT 卷数据）但现在有管理员权限 → 用 MFT 重建固定盘，
            // 否则会一直用无实时更新的遍历数据
            if is_admin {
                let candidates = volume::candidate_drives();
                slog(&format!(
                    "启动: 管理员模式, 快照 {} 卷(全遍历), 待 MFT 重建盘: {:?}",
                    self.index.read().unwrap().volumes.len(),
                    candidates
                ));
                let mut upgraded = false;
                for d in candidates {
                    let has_mft = self
                        .index
                        .read()
                        .unwrap()
                        .volumes
                        .iter()
                        .any(|v| v.letter == d && v.is_ntfs);
                    if has_mft {
                        continue;
                    }
                    if let Some((vi, h, _, _)) = rebuild_volume_aligned(d) {
                        let _ = close_h(h);
                        let mut idx = self.index.write().unwrap();
                        idx.volumes.retain(|v| !(v.letter == d && !v.is_ntfs));
                        idx.volumes.push(vi);
                        upgraded = true;
                        slog(&format!("升级[{d}]: 已替换为 MFT 卷"));
                    }
                }
                if upgraded {
                    let mut idx = self.index.write().unwrap();
                    mark_all_start_menu(&mut idx);
                    let _ = snapshot::save_snapshot(&idx, &snap_path);
                    slog("升级: 快照已更新");
                }
            } else {
                slog("启动: 无管理员权限 → 目录遍历降级模式");
            }
        }

        // 2) 无快照 → 全量构建
        if !loaded {
            let mut idx = Index::new();
            let mut built: Vec<char> = Vec::new();
            if is_admin {
                for d in volume::candidate_drives() {
                    match rebuild_volume_aligned(d) {
                        Some((vi, h, _, _)) => {
                            let _ = close_h(h);
                            built.push(d);
                            idx.volumes.push(vi);
                        }
                        None => {}
                    }
                }
            }
            // 降级覆盖：管理员模式下未被 MFT 覆盖的盘（非 NTFS 可移动盘等）仍走目录扫描
            let walk_letters: Vec<char> = (b'A'..=b'Z')
                .map(|c| c as char)
                .filter(|d| !built.contains(d))
                .filter(|d| {
                    let root = format!("{d}:\\");
                    std::path::Path::new(&root).exists()
                })
                .filter(|d| !is_admin || volume::fixed_drives().contains(d))
                .collect();
            slog(&format!(
                "全量构建: MFT 卷 {built:?}, 遍历卷 {walk_letters:?}"
            ));
            for d in &walk_letters {
                let roots: Vec<String> = if *d == 'C' && is_admin {
                    continue;
                } else if *d == 'C' {
                    default_degraded_roots()
                        .into_iter()
                        .filter(|p| p.starts_with('C'))
                        .collect()
                } else {
                    vec![format!("{d}:\\")]
                };
                for r in roots {
                    let mut vi = VolumeIndex::new(*d, false);
                    let skip = vec!["appdata".to_string()];
                    walker::scan_directory(&r, &skip, &mut |p, is_dir| {
                        vi.push_path_entry(p, is_dir);
                    });
                    idx.volumes.push(vi);
                }
            }
            slog(&format!("全量构建: MFT 卷 {built:?}, 遍历卷 {:?}", &walk_letters));
            *self.index.write().unwrap() = idx;
            mark_all_start_menu(&mut self.index.write().unwrap());
            let _ = snapshot::save_snapshot(&self.index.read().unwrap(), &snap_path);
        }

        // 3) NTFS 卷启动 USN 监听（快照过期的卷先重建）
        let mut live = Vec::new();
        if is_admin {
            let letters: Vec<char> = self
                .index
                .read()
                .unwrap()
                .volumes
                .iter()
                .filter(|v| v.is_ntfs)
                .map(|v| v.letter)
                .collect();
            for d in letters {
                let (snap_jid, snap_usn) = {
                    let idx = self.index.read().unwrap();
                    match idx.volumes.iter().find(|v| v.letter == d) {
                        Some(v) if loaded => (v.journal_id, v.next_usn),
                        Some(v) => (v.journal_id, v.next_usn),
                        None => continue,
                    }
                };
                let stale = stale_vols.contains(&d);
                let (h, jid, resume) = if stale {
                    let _ = close_h(match mft::open_volume(d) {
                        Ok(h) => h,
                        Err(_) => continue,
                    });
                    match rebuild_volume_aligned(d) {
                        Some((vi, h, jid, next)) => {
                            let mut idx = self.index.write().unwrap();
                            idx.volumes.retain(|v| v.letter != d);
                            idx.volumes.push(vi);
                            (h, jid, next)
                        }
                        None => continue,
                    }
                } else {
                    let Ok(h) = mft::open_volume(d) else { continue };
                    if usn::ensure_journal(h).is_err() || usn::query_journal(h).is_err() {
                        let _ = close_h(h);
                        continue;
                    }
                    let Ok((jid, first, _next)) = usn::query_journal(h) else {
                        let _ = close_h(h);
                        continue;
                    };
                    // 快照的日志 ID 不符或起点被回绕丢弃 → 该卷重建
                    if jid != snap_jid || first > snap_usn {
                        let _ = close_h(h);
                        match rebuild_volume_aligned(d) {
                            Some((vi, h2, jid2, next2)) => {
                                let mut idx = self.index.write().unwrap();
                                idx.volumes.retain(|v| v.letter != d);
                                idx.volumes.push(vi);
                                (h2, jid2, next2)
                            }
                            None => continue,
                        }
                    } else {
                        (h, jid, snap_usn)
                    }
                };
                let engine = Arc::clone(self);
                let h = SendHandle(h);
                let gen = self.usn_gen.load(Ordering::SeqCst);
                let t = std::thread::spawn(move || usn_loop(engine, d, h, jid, resume, gen));
                self.threads.lock().unwrap().push(t);
                live.push(d);
                slog(&format!("USN[{d}]: 实时监听已启动, 游标 {resume}"));
            }
            if live.is_empty() {
                slog("USN: 无任何卷启动实时监听！");
            }
            if stale_vols.is_empty() && loaded {
                // 快照有效，无需处理
            }
        }

        // 4) 更新统计
        let (entries, vols) = {
            let idx = self.index.read().unwrap();
            (
                idx.total_entries(),
                idx.volumes
                    .iter()
                    .map(|v| (v.letter, v.is_ntfs, v.len()))
                    .collect::<Vec<_>>(),
            )
        };
        let st = EngineStats {
            entries,
            volumes: vols.clone(),
            from_snapshot: loaded,
            startup_ms: t0.elapsed().as_millis(),
            is_admin,
            usn_live: live.clone(),
        };
        slog(&format!(
            "启动完成: {} 项, 卷 {:?}, usn_live {:?}, 耗时 {}ms",
            entries, vols, live, st.startup_ms
        ));
        *self.stats.write().unwrap() = st.clone();
        st
    }

    /// 搜索（供 UI 每次按键调用；自动作废上一次未完成的扫描）。
    pub fn search(&self, query: &str, opts: &SearchOptions) -> SearchOutcome {
        // query_gen 兼任取消代数：本查询 ID 为自增后的新值，之后任何新搜索都会使 load > gen
        let gen = self.query_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let idx = self.index.read().unwrap();
        matcher::search(&idx, query, Some((&self.query_gen, gen)), opts)
    }

    /// 按配置重建索引：停 USN 线程 → 重建（跳过禁用盘，剔除排除目录）→ 重启 USN → 存快照。
    /// `excluded` 需已归一化为小写 + 反斜杠结尾的可选形式（调用方用 normalize_excluded）。
    pub fn rebuild(self: &Arc<Self>, disabled: &[char], excluded: &[String]) -> EngineStats {
        let t0 = Instant::now();

        // 1) 停旧 USN 线程（代数自增使循环退出，join 等待）
        self.usn_gen.fetch_add(1, Ordering::SeqCst);
        let threads: Vec<_> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }

        // 2) 全量重建
        let is_admin = mft::open_volume('C').is_ok();
        let mut idx = Index::new();
        let mut built: Vec<char> = Vec::new();
        if is_admin {
            for d in volume::candidate_drives() {
                if disabled.contains(&d) {
                    continue;
                }
                match rebuild_volume_aligned(d) {
                    Some((mut vi, h, _, _)) => {
                        let _ = close_h(h);
                        if !excluded.is_empty() {
                            filter_volume_excluded(&mut vi, excluded);
                        }
                        built.push(d);
                        idx.volumes.push(vi);
                    }
                    None => {}
                }
            }
        }
        // 降级扫描卷（未被 MFT 覆盖的盘）
        let walk_letters: Vec<char> = (b'A'..=b'Z')
            .map(|c| c as char)
            .filter(|d| !disabled.contains(d))
            .filter(|d| std::path::Path::new(&format!("{d}:\\")).exists())
            .filter(|d| !built.contains(d))
            .filter(|d| !is_admin || volume::fixed_drives().contains(d))
            .collect();
        slog(&format!(
            "重建: MFT 卷 {built:?}, 遍历卷 {walk_letters:?}"
        ));
        for d in &walk_letters {
            let roots: Vec<String> = if *d == 'C' && is_admin {
                continue;
            } else if *d == 'C' {
                default_degraded_roots()
                    .into_iter()
                    .filter(|p| p.starts_with('C'))
                    .collect()
            } else {
                vec![format!("{d}:\\")]
            };
            for r in roots {
                let mut vi = VolumeIndex::new(*d, false);
                let skip = vec!["appdata".to_string()];
                walker::scan_directory(&r, &skip, &mut |p, is_dir| {
                    if is_excluded_prefix(p, excluded) {
                        return;
                    }
                    vi.push_path_entry(p, is_dir);
                });
                idx.volumes.push(vi);
            }
        }
        *self.index.write().unwrap() = idx;
        mark_all_start_menu(&mut self.index.write().unwrap());
        let snap_path = self.snapshot_path();
        let _ = snapshot::save_snapshot(&self.index.read().unwrap(), &snap_path);

        // 3) 重启 USN 监听（全部是新对齐的游标）
        let mut live = Vec::new();
        if is_admin {
            let letters: Vec<char> = self
                .index
                .read()
                .unwrap()
                .volumes
                .iter()
                .filter(|v| v.is_ntfs)
                .map(|v| v.letter)
                .collect();
            for d in letters {
                let (jid, usn) = {
                    let idx = self.index.read().unwrap();
                    match idx.volumes.iter().find(|v| v.letter == d) {
                        Some(v) => (v.journal_id, v.next_usn),
                        None => continue,
                    }
                };
                let Ok(h) = mft::open_volume(d) else { continue };
                if usn::ensure_journal(h).is_err() || usn::query_journal(h).is_err() {
                    let _ = close_h(h);
                    continue;
                }
                let Ok((jid2, first, next)) = usn::query_journal(h) else {
                    let _ = close_h(h);
                    continue;
                };
                // 快照游标仍有效则续传，否则从日志末尾开始
                let resume = if jid2 == jid && first <= usn { usn } else { next };
                let engine = Arc::clone(self);
                let gen = self.usn_gen.load(Ordering::SeqCst);
                let h = SendHandle(h);
                let t = std::thread::spawn(move || usn_loop(engine, d, h, jid2, resume, gen));
                self.threads.lock().unwrap().push(t);
                live.push(d);
            }
        }

        // 4) 更新统计
        let (entries, vols) = {
            let idx = self.index.read().unwrap();
            (
                idx.total_entries(),
                idx.volumes
                    .iter()
                    .map(|v| (v.letter, v.is_ntfs, v.len()))
                    .collect::<Vec<_>>(),
            )
        };
        let st = EngineStats {
            entries,
            volumes: vols,
            from_snapshot: false,
            startup_ms: t0.elapsed().as_millis(),
            is_admin,
            usn_live: live,
        };
        *self.stats.write().unwrap() = st.clone();
        st
    }

    /// USN 线程每应用一次增量后调用，供诊断。
    pub fn applied(&self) -> u64 {
        self.applied_updates.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> EngineStats {
        self.stats.read().unwrap().clone()
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let threads: Vec<_> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        let path = self.snapshot_path();
        let _ = snapshot::save_snapshot(&self.index.read().unwrap(), &path);
    }
}

/// 降级模式（无管理员）默认扫描的根：C 盘用户目录 + 其他盘根。
fn default_degraded_roots() -> Vec<String> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("USERPROFILE") {
        for d in ["Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"] {
            let p = format!("{home}\\{d}");
            if walker::dir_exists(&p) {
                roots.push(p);
            }
        }
    }
    roots
}

/// 排除前缀是否命中路径（excluded 为小写 + `\` 分隔；目录级命中需整段匹配）。
fn is_excluded_prefix(path: &str, excluded: &[String]) -> bool {
    if excluded.is_empty() {
        return false;
    }
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    excluded.iter().any(|e| {
        lower.starts_with(e)
            && (lower.len() == e.len() || lower[e.len()..].starts_with('\\'))
    })
}

/// 构建后剔除排除目录下的条目（MFT 模式：父链建好后才能算路径）。
fn filter_volume_excluded(vi: &mut VolumeIndex, excluded: &[String]) {
    let mut removed: Vec<u64> = Vec::new();
    for s in 0..vi.len() as u32 {
        if is_excluded_prefix(&vi.path_for_slot(s), excluded) {
            removed.push(vi.frns[s as usize]);
        }
    }
    for frn in removed {
        if frn != 0 {
            vi.remove(frn);
        }
    }
}
