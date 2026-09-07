//! searchcore 基准工具。
//!
//! 用法：
//!   searchcore-bench                    自动模式（管理员→MFT 全盘；否则目录扫描降级）
//!   searchcore-bench --mft              强制 MFT（需要管理员）
//!   searchcore-bench --path D:\某目录   降级扫描指定目录（可多次）
//!   searchcore-bench --iters 30 --limit 50
//!   searchcore-bench --dump 20          打印索引样本核对
//!
//! 验收目标：百万级条目下查询引擎侧 P99 < 10ms。

use std::time::Instant;

use searchcore::index::{Index, VolumeIndex};
use searchcore::matcher::{search, SearchOptions};
use searchcore::mft;
use searchcore::volume;
use searchcore::walker;

fn process_ram_mb() -> f64 {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = GetProcessMemoryInfo(
            HANDLE(GetCurrentProcess().0),
            &mut pmc as *mut _ as *mut _,
            pmc.cb,
        );
        if ok.is_ok() {
            pmc.PrivateUsage as f64 / 1024.0 / 1024.0
        } else {
            0.0
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let force_mft = args.iter().any(|a| a == "--mft");
    let dump: usize = args
        .iter()
        .position(|a| a == "--dump")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let iters: usize = args
        .iter()
        .position(|a| a == "--iters")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let limit: usize = args
        .iter()
        .position(|a| a == "--limit")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    println!("=== searchcore bench ===");

    let ram0 = process_ram_mb();
    let mut index = Index::new();
    let mut built = false;

    // 管理员探测：能否打开 C: 卷
    let is_admin = mft::open_volume('C').is_ok();
    println!("管理员权限: {is_admin}");

    let paths_arg: Vec<String> = args
        .iter()
        .position(|a| a == "--path")
        .and_then(|i| args.get(i + 1))
        .map(|p| vec![p.clone()])
        .unwrap_or_default();

    if is_admin || force_mft {
        for v in volume::list_volumes() {
            let Some(d) = v.drive else { continue };
            if !v.is_ntfs {
                println!("[{d}:] 非 NTFS，跳过 MFT（可后续加目录扫描）");
                continue;
            }
            let t0 = Instant::now();
            match mft::open_volume(d) {
                Ok(h) => {
                    let mut vi = VolumeIndex::new(d, true);
                    let mut n: usize = 0;
                    let r = mft::scan_mft(h, &mut |frn, parent, name, is_dir| {
                        if frn == 5 || name.is_empty() {
                            return; // 根目录/空名不入索引
                        }
                        vi.upsert(frn, parent, name, is_dir);
                        n += 1;
                        if n % 500_000 == 0 {
                            eprintln!("  [{d}:] 已扫描 {n} 条...");
                        }
                    });
                    match r {
                        Ok(_) => {
                            let dt = t0.elapsed();
                            println!(
                                "[{d}:] MFT 扫描完成: {n} 条, 耗时 {:.2}s ({:.0} 万条/s)",
                                dt.as_secs_f64(),
                                n as f64 / dt.as_secs_f64() / 10_000.0
                            );
                            index.volumes.push(vi);
                            built = true;
                        }
                        Err(e) => println!("[{d}:] MFT 扫描失败: {e}"),
                    }
                }
                Err(e) => println!("[{d}:] 打不开卷: {e}"),
            }
        }
    }

    if !built && !force_mft {
        // 降级：目录树扫描（默认根 + --path 指定目录一起压测）
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let mut roots = vec![
            format!("{home}\\Desktop"),
            format!("{home}\\Documents"),
            format!("{home}\\Downloads"),
            format!("{home}\\Pictures"),
            format!("{home}\\Videos"),
            "D:\\".into(),
        ];
        roots.extend(paths_arg);
        roots.retain(|p| walker::dir_exists(p));

        if !roots.is_empty() {
            let letter = 'C';
            let t0 = Instant::now();
            let mut vi = VolumeIndex::new(letter, false);
            let mut n: usize = 0;
            let mut skip: Vec<String> = vec!["appdata".into()];
            for r in &roots {
                println!("[{letter}:] 扫描 {r} ...");
                let st = walker::scan_directory(r, &skip, &mut |p, is_dir| {
                    vi.push_path_entry(p, is_dir);
                    n += 1;
                    if n % 200_000 == 0 {
                        eprintln!("  [{letter}:] 已扫描 {n} 条...");
                    }
                });
                println!("    {} 文件 / {} 目录", st.files, st.dirs);
            }
            let dt = t0.elapsed();
            println!(
                "[{letter}:] 目录扫描完成: {n} 条, 耗时 {:.2}s",
                dt.as_secs_f64()
            );
            index.volumes.push(vi);
            built = true;
        }
    }

    if !built {
        println!("没有可索引的卷/目录，退出");
        return;
    }

    let total = index.total_entries();
    let name_units: usize = index
        .volumes
        .iter()
        .map(|v| v.names.len() * 2 + v.entries.len() * 16 + v.frns.len() * 8 + v.frn_map.len() * 24)
        .sum();
    let ram1 = process_ram_mb();
    println!("------------------------------");
    println!("索引总条目: {total}");
    println!("索引结构占用: {:.1} MB", name_units as f64 / 1024.0 / 1024.0);
    println!("进程内存增量: {:.1} MB", ram1 - ram0);
    println!("------------------------------");

    if dump > 0 {
        for (vi, v) in index.volumes.iter().enumerate() {
            println!("--- 卷 {} [{}] 前 {dump} 条样本 ---", v.letter, vi);
            for e in v.entries.iter().take(dump) {
                let s = v.name_slice(e);
                println!(
                    "  {:?} dir={} path={}",
                    String::from_utf16_lossy(
                        &s[..s.len().min(80)]
                    ),
                    e.is_dir(),
                    e.is_path_entry()
                );
            }
        }
    }

    // 查询基准
    let queries = [
        "pdf", "node", "python", "report", "config", "启动", "报告", "2024", "exe", "main",
    ];
    let opts = SearchOptions {
        limit,
        ..Default::default()
    };

    println!(
        "\n查询基准: {} 个查询 × {} 轮, limit={limit}",
        queries.len(),
        iters
    );
    let mut all: Vec<f64> = Vec::new();
    for q in queries {
        // 预热
        let _ = search(&index, q, None, &opts);
        let mut times: Vec<f64> = Vec::new();
        for _ in 0..iters {
            let t0 = Instant::now();
            let out = search(&index, q, None, &opts);
            let dt = t0.elapsed().as_secs_f64() * 1000.0;
            times.push(dt);
            all.push(dt);
            std::hint::black_box(&out);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |f: f64| times[((times.len() as f64 - 1.0) * f).round() as usize];
        println!(
            "  {q:8} P50={:.2}ms  P95={:.2}ms  P99={:.2}ms  min={:.2}ms",
            p(0.50),
            p(0.95),
            p(0.99),
            times[0]
        );
    }
    all.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |f: f64| all[((all.len() as f64 - 1.0) * f).round() as usize];
    println!(
        "  合计     P50={:.2}ms  P95={:.2}ms  P99={:.2}ms",
        p(0.50),
        p(0.95),
        p(0.99)
    );

    // 样例结果（核对正确性）
    for q in ["pdf", "启动"] {
        let out = search(&index, q, None, &opts);
        println!("\n「{q}」Top {}:", out.items.len().min(8));
        for r in out.items.iter().take(8) {
            println!("  [{:5}] {} {}", r.score, r.name, "  ←  ".to_string() + &r.path);
        }
    }
}
