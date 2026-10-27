use std::fs;
use std::path::PathBuf;
use std::process::Command;

use darak_core::remove::{RecycleBin, RecycleBlock, protected_reason, recycle_block, still_there};
use darak_core::tree::{NO_NODE, Node};
use darak_core::{Kind, Tree, flags};

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

fn tree_with_child(root: &str, name: &str, kind: Kind, flags: u8) -> Tree {
    let mut tree = Tree::default();
    for (text, parent, kind, flags) in [
        (root, NO_NODE, Kind::Dir, 0),
        (name, Tree::ROOT, kind, flags),
    ] {
        let name_off = tree.names.len() as u32;
        tree.names.push_str(text);
        tree.nodes.push(Node {
            alloc: 0,
            logical: 0,
            name_off,
            parent,
            first_child: 0,
            child_count: 0,
            files: 0,
            mtime: 0,
            name_len: text.len() as u16,
            kind,
            flags,
        });
    }
    tree.nodes[0].first_child = 1;
    tree.nodes[0].child_count = 1;
    tree
}

fn reason_for_path(path: &str) -> Option<&'static str> {
    let (root, name) = path.rsplit_once('\\').unwrap();
    let root = if root.ends_with(':') {
        format!("{root}\\")
    } else {
        root.to_owned()
    };
    protected_reason(&tree_with_child(&root, name, Kind::Dir, 0), 1)
}

fn ordinary_path(path: String) -> String {
    path.strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .trim_end_matches('\\')
        .to_owned()
}

#[test]
fn protects_equivalent_absolute_paths() {
    let drive = ordinary_path(std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()));
    let windows =
        ordinary_path(std::env::var("WINDIR").unwrap_or_else(|_| format!("{drive}\\Windows")));
    let home = ordinary_path(std::env::var("USERPROFILE").expect("Windows USERPROFILE"));
    let paths = [
        windows.clone(),
        format!("{windows}\\System32\\kernel32.dll"),
        format!("{drive}\\Program Files"),
        format!("{drive}\\Program Files (x86)"),
        format!("{drive}\\ProgramData"),
        home.clone(),
        format!("{home}\\AppData\\Local"),
    ];
    for path in paths {
        let expected =
            reason_for_path(&path).unwrap_or_else(|| panic!("unprotected ordinary path: {path}"));
        for equivalent in [
            format!(r"\\?\{path}"),
            format!("{}\\", path.to_uppercase()),
            format!("{}\\", format!(r"\\?\{path}").to_uppercase()),
        ] {
            assert_eq!(reason_for_path(&equivalent), Some(expected), "{equivalent}");
        }
    }
}

#[test]
fn protects_root_system_files_in_both_namespaces() {
    for drive in [r"C:\", r"Z:\", r"\\?\C:\", r"\\?\Z:\"] {
        for name in [
            "pagefile.sys",
            "HIBERFIL.SYS",
            "swapfile.sys",
            "dumpstack.log",
            "$Mft",
            "$LogFile",
        ] {
            assert_eq!(
                protected_reason(&tree_with_child(drive, name, Kind::File, 0), 1),
                Some("시스템 파일"),
                "{drive}{name}"
            );
        }
        assert!(
            protected_reason(&tree_with_child(drive, "ordinary.txt", Kind::File, 0), 1).is_none()
        );
    }
}

#[test]
fn allows_neighboring_names_and_nondefault_children() {
    let drive = ordinary_path(std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()));
    let windows =
        ordinary_path(std::env::var("WINDIR").unwrap_or_else(|_| format!("{drive}\\Windows")));
    let home = ordinary_path(std::env::var("USERPROFILE").expect("Windows USERPROFILE"));
    for path in [
        format!("{windows}-old\\ordinary.txt"),
        format!("{drive}\\Program Files-old\\ordinary.txt"),
        format!("{drive}\\Program Files\\MyApp"),
        format!("{home}-old\\ordinary.txt"),
        format!("{home}\\ordinary.txt"),
        format!("{home}\\Downloads\\ordinary.txt"),
    ] {
        for path in [path.clone(), format!(r"\\?\{path}")] {
            assert!(reason_for_path(&path).is_none(), "{path}");
        }
    }
}

#[test]
fn blocks_unsupported_paths() {
    for root in [
        r"\\server\share",
        r"\\?\UNC\server\share",
        r"\\.\C:\folder",
        r"\\?\Volume{123}\folder",
        r"\\?\GLOBALROOT\Device\HarddiskVolume1",
        r"C:relative",
        r"relative",
        r"\rooted",
        r"C:\safe\.\folder",
        r"C:\safe\..\folder",
        r"\\?\C:\safe\..\folder",
        r"C:\safe\\folder",
        r"C:\safe:stream",
        r"C:\safe.",
        r"C:\safe ",
        "C:\\safe\0folder",
    ] {
        assert!(
            protected_reason(&tree_with_child(root, "ordinary.txt", Kind::File, 0), 1).is_some(),
            "{root:?}"
        );
    }
}

#[test]
fn blocks_lossy_ancestry() {
    let mut tree = tree_with_child(
        r"Z:\darak-safe",
        "broken\u{fffd}",
        Kind::Dir,
        flags::LOSSY_NAME,
    );
    assert!(protected_reason(&tree, 1).is_some(), "lossy selected node");
    let mut child = tree.nodes[1];
    child.parent = 1;
    child.kind = Kind::File;
    child.flags = 0;
    child.name_off = tree.names.len() as u32;
    child.name_len = "ordinary.txt".len() as u16;
    tree.names.push_str("ordinary.txt");
    tree.nodes.push(child);
    tree.nodes[1].first_child = 2;
    tree.nodes[1].child_count = 1;
    assert!(protected_reason(&tree, 2).is_some(), "lossy parent");
    tree.nodes[1].flags = 0;
    tree.nodes[0].flags = flags::LOSSY_NAME;
    assert!(
        protected_reason(&tree, 2).is_some(),
        "lossy scan root ancestor"
    );
}

#[test]
fn accepts_valid_replacement_name() {
    for name in ["valid\u{fffd}.txt", "valid\u{1f600}.txt"] {
        assert!(
            protected_reason(&tree_with_child(r"Z:\darak-safe", name, Kind::File, 0), 1).is_none(),
            "{name}"
        );
    }
}

#[test]
fn keeps_scan_root_and_links_protected() {
    let tree = tree_with_child(r"Z:\darak-safe", "link", Kind::Link, 0);
    assert!(protected_reason(&tree, Tree::ROOT).is_some());
    assert!(protected_reason(&tree, 1).is_some());
}

#[test]
fn recycle_block_follows_volume_path_and_capacity() {
    const NO_BIN: &str = "네트워크 드라이브에는 휴지통이 없습니다";
    let open = RecycleBin::Available { capacity: None };
    // Z:\darak-safe\ (14자) + 이름
    let short = tree_with_child(r"Z:\darak-safe", &"a".repeat(245), Kind::Dir, 0);
    let long = tree_with_child(r"Z:\darak-safe", &"a".repeat(246), Kind::Dir, 0);
    assert_eq!(short.path(1).len(), 259);
    assert_eq!(recycle_block(&short, 1, open), None);
    assert_eq!(recycle_block(&long, 1, open), Some(RecycleBlock::LongPath));
    for tree in [&short, &long] {
        for id in [Tree::ROOT, 1] {
            assert_eq!(
                recycle_block(tree, id, RecycleBin::Unavailable(NO_BIN)),
                Some(RecycleBlock::NoBin(NO_BIN))
            );
        }
    }

    let mut tree = tree_with_child(r"Z:\darak-safe", "big", Kind::Dir, 0);
    tree.nodes[1].alloc = 1000;
    tree.nodes[1].logical = 10;
    let bin = |capacity| RecycleBin::Available { capacity: Some(capacity) };
    assert_eq!(recycle_block(&tree, 1, bin(1000)), None);
    assert_eq!(
        recycle_block(&tree, 1, bin(999)),
        Some(RecycleBlock::TooLarge { capacity: 999 })
    );
    assert_eq!(recycle_block(&tree, 1, open), None);
    // 논리 크기가 더 크면 (압축·sparse의 반대) 그쪽을 기준으로 한다
    tree.nodes[1].alloc = 10;
    tree.nodes[1].logical = 2000;
    assert_eq!(
        recycle_block(&tree, 1, bin(1999)),
        Some(RecycleBlock::TooLarge { capacity: 1999 })
    );
    assert_eq!(recycle_block(&tree, 1, bin(2000)), None);
}

#[test]
fn recycle_block_reasons() {
    assert_eq!(RecycleBlock::NoBin("이유").reason(), "이유");
    assert_eq!(
        RecycleBlock::LongPath.reason(),
        "경로가 260자 이상이라 휴지통으로 보낼 수 없습니다"
    );
    assert_eq!(
        RecycleBlock::TooLarge { capacity: 1 << 30 }.reason(),
        "휴지통 최대 크기(1.00 GB)보다 큽니다"
    );
}

#[test]
fn still_there_checks_existence_and_kind() {
    let t = TempDir::new("still-there");
    let dir = t.0.join("folder");
    let file = t.0.join("file.txt");
    let link = t.0.join("link");
    fs::create_dir(&dir).unwrap();
    fs::write(&file, b"x").unwrap();
    let junction = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&dir)
        .output()
        .unwrap();
    assert!(junction.status.success(), "{junction:?}");
    let s = |p: &PathBuf| p.to_str().unwrap().to_owned();

    still_there(&s(&dir), true, false).unwrap();
    still_there(&s(&file), false, false).unwrap();
    still_there(&s(&file), false, true).unwrap();

    let err = still_there(&s(&file), true, false).unwrap_err();
    assert!(err.to_string().contains("종류가 바뀐"), "{err}");
    let err = still_there(&s(&dir), false, false).unwrap_err();
    assert!(err.to_string().contains("종류가 바뀐"), "{err}");

    let err = still_there(&s(&t.0.join("missing")), false, false).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("이미 없는"), "{err}");
    let err = still_there(&s(&t.0.join(r"missing\child")), true, false).unwrap_err();
    assert!(err.to_string().contains("이미 없는"), "{err}");

    // 스캔 때 폴더였던 자리가 junction으로 바뀐 경우
    let err = still_there(&s(&link), true, false).unwrap_err();
    assert!(err.to_string().contains("링크로 바뀐"), "{err}");
    // 대상은 그대로 남아 있어야 한다
    assert!(dir.is_dir() && file.is_file());
}
