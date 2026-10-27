use std::fs;
use std::path::PathBuf;
use std::process::Command;

use darak_core::{Kind, ScanOptions, ScanProgress, Tree, flags, scan};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("darak-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // junction은 remove_dir_all이 대상 안으로 들어가지 않고 링크만 지운다
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn find(tree: &Tree, path: &[&str]) -> u32 {
    let mut cur = Tree::ROOT;
    for part in path {
        cur = tree
            .children(cur)
            .find(|&c| tree.name(c) == *part)
            .unwrap_or_else(|| panic!("{part} 없음"));
    }
    cur
}

#[test]
fn scans_sizes_hardlinks_and_junctions() {
    let t = TempDir::new("basic");
    let root = &t.0;
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::create_dir_all(root.join("big")).unwrap();
    fs::write(root.join("a/one.txt"), vec![1u8; 10_000]).unwrap();
    fs::write(root.join("a/b/two.bin"), vec![2u8; 50_000]).unwrap();
    fs::write(root.join("big/large.dat"), vec![3u8; 300_000]).unwrap();
    fs::write(root.join("한글 이름.txt"), b"hello").unwrap();
    fs::hard_link(root.join("big/large.dat"), root.join("a/large-link.dat")).unwrap();
    let junction = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("loop"))
        .arg(root.join("big"))
        .output()
        .unwrap();
    assert!(junction.status.success(), "{junction:?}");

    let progress = ScanProgress::default();
    let res = scan(root.to_str().unwrap(), &ScanOptions::default(), &progress).unwrap();
    let tree = &res.tree;
    assert!(!res.stats.cancelled && tree.nodes.iter().all(|n| !n.incomplete()), "complete walk has no unread folders");

    let r = tree.node(Tree::ROOT);
    assert_eq!(r.logical, 10_000 + 50_000 + 300_000 + 5, "하드링크는 한 번만, junction은 따라가지 않음");
    assert_eq!(r.files, 5);
    assert_eq!(res.stats.hardlink_dups, 1);

    // 자식은 크기 내림차순
    let kids: Vec<u64> = tree.children(Tree::ROOT).map(|c| tree.node(c).alloc).collect();
    assert!(kids.windows(2).all(|w| w[0] >= w[1]), "{kids:?}");

    let lnk = find(tree, &["loop"]);
    assert_eq!(tree.node(lnk).kind, Kind::Link);
    assert_eq!(tree.node(lnk).child_count, 0);

    let two = find(tree, &["a", "b", "two.bin"]);
    assert_eq!(tree.node(two).logical, 50_000);
    assert!(tree.path(two).ends_with(r"\a\b\two.bin"));
    assert!(tree.node(two).mtime > 0);

    find(tree, &["한글 이름.txt"]);

    // 하드링크 두 개 중 정확히 하나만 크기를 가진다
    let l1 = tree.node(find(tree, &["big", "large.dat"]));
    let l2 = tree.node(find(tree, &["a", "large-link.dat"]));
    assert_eq!(l1.has(flags::HARDLINK_DUP) as u8 + l2.has(flags::HARDLINK_DUP) as u8, 1);
    assert_eq!(l1.logical + l2.logical, 300_000);
}

#[test]
fn remove_updates_ancestors() {
    let t = TempDir::new("remove");
    fs::create_dir_all(t.0.join("x/y")).unwrap();
    fs::write(t.0.join("x/y/f"), vec![0u8; 4096]).unwrap();
    fs::write(t.0.join("x/g"), vec![0u8; 100]).unwrap();
    let mut tree = scan(t.0.to_str().unwrap(), &ScanOptions::default(), &ScanProgress::default()).unwrap().tree;
    let y = find(&tree, &["x", "y"]);
    tree.remove(y);
    assert_eq!(tree.node(Tree::ROOT).logical, 100);
    assert_eq!(tree.node(find(&tree, &["x"])).files, 1);
    assert!(tree.node(y).has(flags::REMOVED));
    assert_eq!(tree.live_children(find(&tree, &["x"])).count(), 1);
}

#[test]
fn missing_root_is_error() {
    assert!(scan(r"Z:\definitely\not\here", &ScanOptions::default(), &ScanProgress::default()).is_err());
}

#[test]
fn cancelled_walk_returns_marked_empty_root() {
    let t = TempDir::new("cancel");
    fs::create_dir_all(t.0.join("a/b")).unwrap();
    fs::write(t.0.join("a/b/f"), vec![0u8; 4096]).unwrap();
    let progress = ScanProgress::default();
    progress.cancel();
    let opts = ScanOptions { use_mft: false, ..Default::default() };
    let res = scan(t.0.to_str().unwrap(), &opts, &progress).unwrap();
    let root = res.tree.node(Tree::ROOT);
    assert!(res.stats.cancelled);
    assert!(root.has(flags::INCOMPLETE) && !root.has(flags::DENIED), "{:#x}", root.flags);
    assert!(root.incomplete());
    assert_eq!((root.child_count, root.files, root.alloc), (0, 0, 0));
    assert_eq!(res.stats.errors, 0, "unread folders are not access errors");
}

#[test]
fn cancelled_drive_scan_does_not_fall_back_to_walk() {
    // 중지 플래그를 먼저 세우므로 MFT 경로는 볼륨을 열지 않고 끝난다. 일반 순회로 넘어가면
    // 루트만 있는 빈 트리가 Ok로 나온다.
    let temp = std::env::temp_dir();
    let temp = temp.to_str().unwrap();
    assert_eq!(temp.as_bytes()[1], b':', "{temp}");
    let root = format!("{}\\", &temp[..2]);
    let progress = ScanProgress::default();
    progress.cancel();
    let err = scan(&root, &ScanOptions::default(), &progress).err().expect("stopped MFT scan must not walk the drive");
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "{err}");
}

#[test]
fn search_and_protection() {
    use darak_core::remove::{protected_reason, top_level_marks};
    use darak_core::search::{Query, now_secs2000, search};
    use darak_core::SizeMode;

    let t = TempDir::new("search");
    fs::create_dir_all(t.0.join("movies/old")).unwrap();
    fs::write(t.0.join("movies/a.MP4"), vec![0u8; 5000]).unwrap();
    fs::write(t.0.join("movies/old/b.mkv"), vec![0u8; 9000]).unwrap();
    fs::write(t.0.join("notes.txt"), b"x").unwrap();
    let tree = scan(t.0.to_str().unwrap(), &ScanOptions::default(), &ScanProgress::default()).unwrap().tree;
    let q = |s: &str| search(&tree, &Query::parse(s, now_secs2000()).unwrap(), SizeMode::Logical, Tree::ROOT, 100);

    let r = q("ext:mp4;mkv");
    assert_eq!(r.total, 2);
    assert_eq!(tree.name(r.hits[0]), "b.mkv", "크기 내림차순");
    assert_eq!(q("*.mp4").total, 1);
    assert_eq!(q("size:>6000").total, 3, "b.mkv, old, movies");
    assert_eq!(q("size:>6000 file:").total, 1);
    assert_eq!(q("movie").total, 1);
    assert_eq!(q("dm:<1d file:").total, 3);
    assert_eq!(q("path:old").total, 2);
    assert_eq!(q("!mov file:").total, 3);

    assert!(protected_reason(&tree, Tree::ROOT).is_some());
    let movies = find(&tree, &["movies"]);
    let b = find(&tree, &["movies", "old", "b.mkv"]);
    assert!(protected_reason(&tree, movies).is_none());
    assert_eq!(top_level_marks(&tree, &[b, movies]), vec![movies]);
}

#[test]
fn live_snapshot_matches_totals() {
    let t = TempDir::new("live");
    fs::create_dir_all(t.0.join("a/b/c/d/e/f/g")).unwrap();
    fs::write(t.0.join("a/b/c/d/e/f/g/deep.bin"), vec![0u8; 7000]).unwrap();
    fs::write(t.0.join("a/x.bin"), vec![0u8; 3000]).unwrap();
    fs::write(t.0.join("top.bin"), vec![0u8; 1000]).unwrap();
    let progress = ScanProgress::default();
    let res = scan(t.0.to_str().unwrap(), &ScanOptions::default(), &progress).unwrap();
    let snap = progress.live.lock().unwrap().snapshot().unwrap();
    let r = snap.node(Tree::ROOT);
    assert_eq!(r.logical, res.tree.node(Tree::ROOT).logical);
    assert_eq!(r.files, 3);
    // 자식 합계 = 부모 크기 (얕은 깊이에서)
    let sum: u64 = snap.children(Tree::ROOT).map(|c| snap.node(c).logical).sum();
    assert_eq!(sum, r.logical);
    assert!(snap.children(Tree::ROOT).any(|c| snap.name(c) == "파일 1개"));
}
