use darak_core::remove::protected_reason;
use darak_core::tree::{NO_NODE, Node};
use darak_core::{Kind, Tree, flags};

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
