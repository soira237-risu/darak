use darak_core::search::{Query, search};
use darak_core::{Kind, NO_NODE, Node, NodeId, SizeMode, Tree};

fn fixture() -> Tree {
    let mut tree = Tree::default();
    for (name, kind, parent, size) in [
        ("C:\\", Kind::Dir, NO_NODE, 10),
        ("Été.txt", Kind::File, Tree::ROOT, 4),
        ("archive.ÉTÉ", Kind::File, Tree::ROOT, 3),
        ("İ.TXT", Kind::File, Tree::ROOT, 2),
        ("ÉTÉ", Kind::Dir, Tree::ROOT, 1),
        ("child.txt", Kind::File, 4, 1),
    ] {
        tree.nodes.push(Node {
            alloc: size,
            logical: size,
            name_off: tree.names.len() as u32,
            name_len: name.len() as u16,
            parent,
            first_child: if kind == Kind::Dir {
                if parent == NO_NODE { 1 } else { 5 }
            } else {
                0
            },
            child_count: if kind == Kind::Dir {
                if parent == NO_NODE { 4 } else { 1 }
            } else {
                0
            },
            files: if parent == NO_NODE { 4 } else { 1 },
            mtime: 0,
            kind,
            flags: 0,
        });
        tree.names.push_str(name);
    }
    tree
}

fn hits(tree: &Tree, query: &str) -> Vec<NodeId> {
    let query = Query::parse(query, 0).unwrap();
    let mut ids = search(tree, &query, SizeMode::Logical, Tree::ROOT, usize::MAX).hits;
    ids.sort_unstable();
    ids
}

#[test]
fn matches_unicode_search_terms() {
    let tree = fixture();
    for query in ["ÉTÉ", "été"] {
        assert_eq!(hits(&tree, query), vec![1, 2, 4], "{query}");
    }
    assert_eq!(hits(&tree, "!ÉTÉ"), vec![3, 5]);
    assert_eq!(hits(&tree, "path:ÉTÉ"), vec![1, 2, 4, 5]);
    assert_eq!(hits(&tree, "ext:ÉTÉ"), vec![2]);
    assert_eq!(hits(&tree, "ext:été;TXT"), vec![1, 2, 3, 5]);
    assert_eq!(hits(&tree, "i\u{307}"), vec![3]);
    assert_eq!(hits(&tree, "!i\u{307}"), vec![1, 2, 4, 5]);
    assert_eq!(hits(&tree, "path:i\u{307}"), vec![3]);
}

#[test]
fn query_wildcards_preserve_original_scalars() {
    let tree = fixture();
    assert_eq!(hits(&tree, "?.TXT"), vec![3]);
    assert_eq!(hits(&tree, "İ*.txt"), vec![3]);
    assert!(hits(&tree, "i\u{307}*.txt").is_empty());
    assert_eq!(hits(&tree, "ÉTÉ.*"), vec![1]);
}

#[test]
fn unicode_results_keep_order_totals_and_scope() {
    let tree = fixture();
    let query = Query::parse("path:ÉTÉ", 0).unwrap();
    let result = search(&tree, &query, SizeMode::Logical, Tree::ROOT, 2);
    assert_eq!(result.hits, vec![1, 2]);
    assert_eq!(result.total, 4);
    assert_eq!(result.total_size, 8);
    let scoped = search(&tree, &query, SizeMode::Logical, 4, usize::MAX);
    assert_eq!(scoped.hits, vec![4, 5]);
    assert_eq!(scoped.total_size, 1);
}

fn sigma_fixture() -> Tree {
    let mut tree = fixture();
    tree.names.clear();
    for (node, name) in tree.nodes.iter_mut().zip(["C:\\", ".Σ", "a.Σ", "a.ΟΣ", "Σ", "child.txt"]) {
        node.name_off = tree.names.len() as u32;
        node.name_len = name.len() as u16;
        tree.names.push_str(name);
    }
    tree
}

#[test]
fn matches_sigma_extension_operands() {
    let tree = sigma_fixture();
    for query in ["ext:Σ", "ext:σ", "EXT:.Σ"] {
        assert_eq!(hits(&tree, query), vec![1, 2], "{query}");
    }
    for query in ["ext:ΟΣ;Σ", "ext:Σ;ΟΣ", "ext:ος,σ"] {
        assert_eq!(hits(&tree, query), vec![1, 2, 3], "{query}");
    }
    assert!(hits(&tree, "ext:ς").is_empty());
    assert!(hits(&tree, "ext:οσ").is_empty());
}

#[test]
fn matches_sigma_path_operands() {
    let tree = sigma_fixture();
    for query in ["path:Σ", "path:σ", "PATH:Σ"] {
        assert_eq!(hits(&tree, query), vec![1, 4, 5], "{query}");
    }
}
