use darak_core::search::{Query, search};
use darak_core::{Kind, NO_NODE, Node, NodeId, SizeMode, Tree, flags};

const FOLDER: NodeId = 1;
const SIBLING: NodeId = 2;
const CHILD1: NodeId = 3;
const CHILD2: NodeId = 4;

fn add_node(tree: &mut Tree, name: &str, parent: NodeId, kind: Kind, totals: (u64, u64, u32)) {
    let name_off = tree.names.len() as u32;
    tree.names.push_str(name);
    tree.nodes.push(Node {
        alloc: totals.0,
        logical: totals.1,
        files: totals.2,
        name_off,
        name_len: name.len() as u16,
        parent,
        first_child: 0,
        child_count: 0,
        mtime: 42,
        kind,
        flags: 0,
    });
}

fn fixture() -> Tree {
    let mut tree = Tree::default();
    add_node(&mut tree, "C:\\fixture", NO_NODE, Kind::Dir, (100, 130, 3));
    add_node(&mut tree, "Folder", Tree::ROOT, Kind::Dir, (60, 80, 2));
    add_node(
        &mut tree,
        "sibling.txt",
        Tree::ROOT,
        Kind::File,
        (40, 50, 1),
    );
    add_node(&mut tree, "child1.txt", FOLDER, Kind::File, (20, 25, 1));
    add_node(&mut tree, "child2.txt", FOLDER, Kind::File, (40, 55, 1));
    tree.nodes[Tree::ROOT as usize].first_child = FOLDER;
    tree.nodes[Tree::ROOT as usize].child_count = 2;
    tree.nodes[FOLDER as usize].first_child = CHILD1;
    tree.nodes[FOLDER as usize].child_count = 2;
    tree.nodes[CHILD2 as usize].flags = flags::REPARSE;
    tree
}

fn totals(tree: &Tree, id: NodeId) -> (u64, u64, u32) {
    let node = tree.node(id);
    (node.alloc, node.logical, node.files)
}

fn assert_removed(tree: &Tree, id: NodeId) {
    assert!(tree.node(id).has(flags::REMOVED), "node {id} remains live");
    assert_eq!(totals(tree, id), (0, 0, 0), "node {id} retains totals");
}

#[test]
fn removes_entire_subtree_once() {
    let mut tree = fixture();
    let names = tree.names.clone();
    let metadata: Vec<_> = tree
        .nodes
        .iter()
        .map(|n| {
            (
                n.parent,
                n.first_child,
                n.child_count,
                n.name_off,
                n.name_len,
                n.kind,
                n.mtime,
            )
        })
        .collect();
    tree.remove(FOLDER);
    assert_eq!(totals(&tree, Tree::ROOT), (40, 50, 1));
    assert_eq!(totals(&tree, SIBLING), (40, 50, 1));
    assert!(!tree.node(SIBLING).has(flags::REMOVED));
    for id in [FOLDER, CHILD1, CHILD2] {
        assert_removed(&tree, id);
    }
    assert!(tree.node(CHILD2).has(flags::REPARSE));
    assert_eq!(
        tree.live_children(Tree::ROOT).collect::<Vec<_>>(),
        vec![SIBLING]
    );
    assert_eq!(tree.live_children(FOLDER).count(), 0);
    assert_eq!(tree.names, names);
    assert_eq!(tree.nodes.len(), metadata.len());
    for (node, before) in tree.nodes.iter().zip(metadata) {
        assert_eq!(
            (
                node.parent,
                node.first_child,
                node.child_count,
                node.name_off,
                node.name_len,
                node.kind,
                node.mtime
            ),
            before
        );
    }
    assert_eq!(std::mem::size_of::<Node>(), 48);
    assert_eq!(tree.version, 1);
    let version = tree.version;
    tree.remove(CHILD1);
    tree.remove(CHILD2);
    tree.remove(FOLDER);
    assert_eq!(tree.version, version);
    assert_eq!(totals(&tree, Tree::ROOT), (40, 50, 1));
}

#[test]
fn keeps_root_and_handles_deep_subtree() {
    const DEPTH: NodeId = 100_000;
    let mut tree = Tree::default();
    for id in 0..=DEPTH {
        add_node(
            &mut tree,
            "n",
            if id == 0 { NO_NODE } else { id - 1 },
            if id == DEPTH { Kind::File } else { Kind::Dir },
            (7, 11, 1),
        );
        if id < DEPTH {
            tree.nodes[id as usize].first_child = id + 1;
            tree.nodes[id as usize].child_count = 1;
        }
    }
    tree.remove(Tree::ROOT);
    assert_eq!(tree.version, 0);
    assert!(
        tree.nodes
            .iter()
            .all(|n| !n.has(flags::REMOVED) && (n.alloc, n.logical, n.files) == (7, 11, 1))
    );
    assert_eq!(tree.remove(1), (1, u64::from(DEPTH) - 1));
    assert_eq!(totals(&tree, Tree::ROOT), (0, 0, 0));
    assert!(!tree.node(Tree::ROOT).has(flags::REMOVED));
    for id in 1..=DEPTH {
        assert_removed(&tree, id);
    }
    assert_eq!(tree.version, 1);
    assert_eq!(tree.remove(DEPTH), (0, 0));
    assert_eq!(tree.remove(1), (0, 0));
    assert_eq!(tree.version, 1);
}

#[test]
fn excludes_removed_descendants_from_search() {
    let mut tree = fixture();
    let query = Query::parse("file:", 0).unwrap();
    assert_eq!(
        search(&tree, &query, SizeMode::Allocated, Tree::ROOT, 10).total,
        3
    );
    tree.remove(FOLDER);
    for mode in [SizeMode::Allocated, SizeMode::Logical] {
        let result = search(&tree, &query, mode, Tree::ROOT, 10);
        assert_eq!(result.hits, vec![SIBLING]);
        assert_eq!(result.total, 1);
        assert_eq!(result.total_size, tree.node(SIBLING).size(mode));
        assert_eq!(search(&tree, &query, mode, FOLDER, 10).total, 0);
    }
}

#[test]
fn removes_child_then_parent_without_double_subtraction() {
    let mut tree = fixture();
    tree.remove(CHILD1);
    assert_eq!(totals(&tree, Tree::ROOT), (80, 105, 2));
    assert_eq!(totals(&tree, FOLDER), (40, 55, 1));
    assert_removed(&tree, CHILD1);
    assert_eq!(tree.version, 1);
    tree.remove(FOLDER);
    assert_eq!(totals(&tree, Tree::ROOT), (40, 50, 1));
    for id in [FOLDER, CHILD1, CHILD2] {
        assert_removed(&tree, id);
    }
    assert_eq!(tree.version, 2);
    tree.remove(CHILD1);
    tree.remove(CHILD2);
    tree.remove(FOLDER);
    assert_eq!(tree.version, 2);
    assert_eq!(totals(&tree, Tree::ROOT), (40, 50, 1));
}

#[test]
fn remove_reports_removed_files_and_dirs() {
    let mut tree = fixture();
    assert_eq!(tree.remove(Tree::ROOT), (0, 0));
    assert_eq!(tree.remove(FOLDER), (2, 1));
    assert_eq!(tree.remove(FOLDER), (0, 0));
    assert_eq!(tree.remove(CHILD2), (0, 0));
    assert_eq!(tree.remove(SIBLING), (1, 0));

    // 자식을 먼저 지우고 부모를 지워도 폴더를 두 번 세지 않는다
    let mut tree = fixture();
    add_node(&mut tree, "Inner", FOLDER, Kind::Dir, (0, 0, 0));
    add_node(&mut tree, "link", FOLDER, Kind::Link, (0, 0, 0));
    tree.nodes[FOLDER as usize].child_count = 4;
    assert_eq!(tree.remove(CHILD1), (1, 0));
    assert_eq!(tree.remove(5), (0, 1));
    assert_eq!(tree.remove(FOLDER), (1, 1), "link는 폴더로 세지 않는다");
    assert!(tree.node(6).has(flags::REMOVED));
}
