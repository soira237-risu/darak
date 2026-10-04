//! 스캔 벤치마크/디버깅용 CLI
//!
//! ```text
//! darak-cli C:\ [--top N] [--logical] [--no-dedupe] [--threads N] [--walk] [--compare]
//! ```

use std::collections::BinaryHeap;
use std::cmp::Reverse;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use darak_core::scan::process_memory;
use darak_core::{Kind, ScanOptions, ScanProgress, SizeMode, Tree, human_size, scan, thousands};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut root = None;
    let mut top = 15usize;
    let mut mode = SizeMode::Allocated;
    let mut opts = ScanOptions::default();
    let mut compare = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--top" => top = args.next().and_then(|v| v.parse().ok()).unwrap_or(top),
            "--logical" => mode = SizeMode::Logical,
            "--no-dedupe" => opts.dedupe_hardlinks = false,
            "--threads" => opts.threads = args.next().and_then(|v| v.parse().ok()),
            "--walk" => opts.use_mft = false,
            "--compare" => compare = true,
            "-h" | "--help" => {
                println!("사용법: darak-cli <경로> [--top N] [--logical] [--no-dedupe] [--threads N] [--walk] [--compare]");
                return;
            }
            _ => root = Some(a),
        }
    }
    let root = root.unwrap_or_else(|| "C:\\".into());

    let progress = Arc::new(ScanProgress::default());
    let ticker = {
        let p = progress.clone();
        std::thread::spawn(move || {
            while !p.is_cancelled() {
                std::thread::sleep(Duration::from_millis(500));
                eprint!(
                    "\r  {} 파일, {} 폴더, {}        ",
                    thousands(p.files.load(Relaxed)),
                    thousands(p.dirs.load(Relaxed)),
                    human_size(p.bytes.load(Relaxed))
                );
            }
        })
    };

    let res = scan(&root, &opts, &progress);
    progress.cancel();
    ticker.join().ok();
    eprintln!();
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            eprintln!("스캔 실패: {root}: {e}");
            std::process::exit(1);
        }
    };
    let tree = &res.tree;
    let s = &res.stats;
    let (ws, peak) = process_memory();
    let r = tree.node(Tree::ROOT);

    println!("경로          {}", tree.name(Tree::ROOT));
    println!("방식          {:?}{}", s.method, s.fallback_reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default());
    println!("소요 시간     {:.2}s", s.elapsed.as_secs_f64());
    println!("파일 / 폴더   {} / {}", thousands(s.files), thousands(s.dirs));
    println!("노드          {}", thousands(tree.len() as u64));
    println!("접근 실패     {}", thousands(s.errors));
    println!("하드링크 중복 {}", thousands(s.hardlink_dups));
    println!("할당 크기     {} ({} B)", human_size(r.alloc), thousands(r.alloc));
    println!("논리 크기     {} ({} B)", human_size(r.logical), thousands(r.logical));
    println!(
        "메모리        트리 {} / working set {} / 최대 {}",
        human_size(tree.heap_bytes() as u64),
        human_size(ws as u64),
        human_size(peak as u64)
    );

    println!("\n── 최상위 항목 ──");
    let mut kids: Vec<u32> = tree.children(Tree::ROOT).collect();
    kids.sort_by_key(|&c| Reverse(tree.node(c).size(mode)));
    for &c in kids.iter().take(top) {
        print_row(tree, c, mode, r.size(mode));
    }

    if compare {
        compare_with_walk(&root, tree, mode, top);
    }

    println!("\n── 가장 큰 파일 ──");
    let mut heap = BinaryHeap::new();
    for (i, n) in tree.nodes.iter().enumerate() {
        if n.kind == Kind::File {
            heap.push(Reverse((n.size(mode), i as u32)));
            if heap.len() > top {
                heap.pop();
            }
        }
    }
    let mut biggest: Vec<_> = heap.into_iter().map(|Reverse(x)| x).collect();
    biggest.sort_by(|a, b| b.cmp(a));
    for (size, id) in biggest {
        println!("{:>10}  {}", human_size(size), tree.path(id));
    }
}

/// 일반 순회 결과와 최상위 항목별 크기를 비교한다 (MFT 파서 검증용)
fn compare_with_walk(root: &str, tree: &Tree, mode: SizeMode, top: usize) {
    let opts = ScanOptions { use_mft: false, ..Default::default() };
    let walk = match scan(root, &opts, &ScanProgress::default()) {
        Ok(r) => r,
        Err(e) => return eprintln!("비교 스캔 실패: {e}"),
    };
    let w = &walk.tree;
    println!("
── 일반 순회와 비교 ({:.2}s) ──", walk.stats.elapsed.as_secs_f64());
    println!("{:>12} {:>12} {:>9}  이름", "이 스캔", "일반 순회", "차이");
    let mut kids: Vec<u32> = tree.children(Tree::ROOT).collect();
    kids.sort_by_key(|&c| Reverse(tree.node(c).size(mode)));
    for &c in kids.iter().take(top.max(20)) {
        let name = tree.name(c);
        let a = tree.node(c).size(mode);
        let b = w.children(Tree::ROOT).find(|&x| w.name(x).eq_ignore_ascii_case(name)).map(|x| w.node(x).size(mode));
        match b {
            Some(b) => {
                let diff = a as i128 - b as i128;
                let mark = if diff.unsigned_abs() > (a.max(b) / 100) as u128 && diff.unsigned_abs() > 1 << 20 { " ◀" } else { "" };
                println!("{:>12} {:>12} {:>9}  {name}{mark}", human_size(a), human_size(b), signed(diff));
            }
            None => println!("{:>12} {:>12} {:>9}  {name}", human_size(a), "-", ""),
        }
    }
    let (a, b) = (tree.node(Tree::ROOT).size(mode), w.node(Tree::ROOT).size(mode));
    println!("{:>12} {:>12} {:>9}  (전체)", human_size(a), human_size(b), signed(a as i128 - b as i128));
}

fn signed(d: i128) -> String {
    let s = human_size(d.unsigned_abs() as u64);
    if d < 0 { format!("-{s}") } else { format!("+{s}") }
}

fn print_row(tree: &Tree, id: u32, mode: SizeMode, total: u64) {
    let n = tree.node(id);
    let pct = if total > 0 { n.size(mode) as f64 * 100.0 / total as f64 } else { 0.0 };
    let tag = match n.kind {
        Kind::Dir => "\\",
        Kind::Link => " ->",
        Kind::File => "",
    };
    println!(
        "{:>10} {:>5.1}%  {}{}  ({} 파일)",
        human_size(n.size(mode)),
        pct,
        tree.name(id),
        tag,
        thousands(n.files as u64)
    );
}
