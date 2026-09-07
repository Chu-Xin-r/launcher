//! M2 验收工具：
//!   engine-test first   全量构建 + 存快照 + 搜索冒烟
//!   engine-test resume  载快照启动（测启动耗时）+ 搜索冒烟
//!   engine-test cycle   模拟 USN 增删改（直接构造记录）验证增量正确性

use std::time::Instant;

use searchcore::engine::Engine;
use searchcore::index::VolumeIndex;
use searchcore::matcher::SearchOptions;
use searchcore::snapshot;
use searchcore::usn::{self, UsnUpdate};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "first".into());
    match mode.as_str() {
        "first" => run(false),
        "resume" => run(true),
        "cycle" => cycle_test(),
        _ => eprintln!("用法: engine-test first|resume|cycle"),
    }
}

fn run(expect_snapshot: bool) {
    let engine = Engine::new();
    engine.set_snapshot_path(std::env::temp_dir().join("launcher-test-index.bin"));
    let st = engine.startup();
    println!(
        "启动: {} 条目, 耗时 {} ms, 来自快照={} (预期={}), 管理员={}, USN监听={:?}",
        st.entries, st.startup_ms, st.from_snapshot, expect_snapshot, st.is_admin, st.usn_live
    );
    let opts = SearchOptions::default();
    let t0 = Instant::now();
    let out = engine.search("pdf", &opts);
    println!(
        "搜索「pdf」: {} 条结果, 耗时 {:.2}ms",
        out.items.len(),
        t0.elapsed().as_secs_f64() * 1000.0
    );
    if let Some(r) = out.items.first() {
        println!("  首条: {} ← {}", r.name, r.path);
    }
    engine.shutdown();
    if expect_snapshot && !st.from_snapshot {
        eprintln!("FAIL: 预期从快照启动");
        std::process::exit(1);
    }
    if !expect_snapshot && st.from_snapshot {
        eprintln!("FAIL: 预期全新构建");
        std::process::exit(1);
    }
    println!("OK");
}

/// 用合成 USN 记录验证增量应用的正确性（建→搜到→删→搜不到→改名→新名搜到）。
fn cycle_test() {
    let tmp = std::env::temp_dir().join("launcher-cycle-test");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // 独立卷索引（非 NTFS 的路径条目无法走 USN；这里直接用 FRN 语义测 apply_updates）
    let mut vi = VolumeIndex::new('Z', true);

    let u = |frn: u64, parent: u64, reason: u32, name: &str, is_dir: bool| UsnUpdate {
        frn,
        parent,
        usn: 0,
        reason,
        is_dir,
        name: name.encode_utf16().collect(),
    };

    // 1) 建两个文件
    usn::apply_updates(
        &mut vi,
        &[
            u(0x1001, 5, usn::REASON_FILE_CREATE, "hello.txt", false),
            u(0x1002, 5, usn::REASON_FILE_CREATE, "world.pdf", false),
        ],
    );
    assert_eq!(vi.len(), 2, "创建后应有 2 条");

    // 2) 改名 hello.txt → goodbye.txt
    usn::apply_updates(
        &mut vi,
        &[
            u(0x1001, 5, usn::REASON_RENAME_OLD, "hello.txt", false),
            u(0x1001, 5, usn::REASON_RENAME_NEW, "goodbye.txt", false),
        ],
    );
    assert_eq!(vi.len(), 2, "改名后仍应 2 条");
    let hit = searchcore::matcher::search(
        &searchcore::Index { volumes: vec![vi] },
        "goodbye",
        None,
        &SearchOptions::default(),
    );
    assert_eq!(hit.items.len(), 1, "新名应可搜到");
    assert!(hit.items[0].path.ends_with("goodbye.txt"), "路径应为新名: {}", hit.items[0].path);

    // 3) 删除 world.pdf（对改名后的索引再操作：重建一下干净状态）
    let mut vi = VolumeIndex::new('Z', true);
    usn::apply_updates(
        &mut vi,
        &[
            u(0x1001, 5, usn::REASON_FILE_CREATE, "hello.txt", false),
            u(0x1002, 5, usn::REASON_FILE_CREATE, "world.pdf", false),
        ],
    );
    usn::apply_updates(&mut vi, &[u(0x1002, 5, usn::REASON_FILE_DELETE, "world.pdf", false)]);
    assert_eq!(vi.len(), 1, "删除后应剩 1 条");
    let idx = searchcore::Index { volumes: vec![vi] };
    let out = searchcore::matcher::search(&idx, "world", None, &SearchOptions::default());
    assert!(out.items.is_empty(), "删除后不应搜到");
    let out = searchcore::matcher::search(&idx, "hello", None, &SearchOptions::default());
    assert_eq!(out.items.len(), 1, "未删除的应仍在");

    // 4) 快照 round-trip
    let snap = std::env::temp_dir().join("launcher-cycle-snap.bin");
    snapshot::save_snapshot(&idx, &snap).unwrap();
    let idx2 = snapshot::load_snapshot(&snap).unwrap();
    assert_eq!(idx2.total_entries(), 1, "快照载入条数一致");
    let out = searchcore::matcher::search(&idx2, "hello", None, &SearchOptions::default());
    assert_eq!(out.items.len(), 1, "快照恢复后可搜");
    assert_eq!(out.items[0].path, "Z:\\hello.txt", "快照恢复路径正确");

    let _ = std::fs::remove_dir_all(&tmp);
    println!("cycle-test OK: 创建/改名/删除/快照 round-trip 全部通过");
}
